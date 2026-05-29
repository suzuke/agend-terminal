//! #1440 invariant: in `build_command` (src/agent/mod.rs) `cmd.env_clear()`
//! must precede any `cmd.env(...)`. If a future edit injects a `cmd.env(...)`
//! before the clear, env isolation would silently wipe the explicitly-set
//! keys — this test fails loud. Mirrors the §10.5 spawn-rationale source-scan
//! invariant style.

use std::path::PathBuf;

#[test]
fn env_clear_precedes_first_env_set_in_build_command() {
    let src_path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/agent/mod.rs");
    let src = std::fs::read_to_string(&src_path).expect("read src/agent/mod.rs");

    // Isolate the build_command body: from its signature to the next top-level
    // fn definition after it.
    let start = src.find("fn build_command").expect("build_command present");
    let after = &src[start..];
    let next_fn = after[1..].find("\nfn ").map(|i| i + 1);
    let next_pub_fn = after[1..].find("\npub fn ").map(|i| i + 1);
    let end_rel = [next_fn, next_pub_fn]
        .into_iter()
        .flatten()
        .min()
        .unwrap_or(after.len());
    // Strip line comments so prose mentioning the call pattern can't false-match.
    let body: String = after[..end_rel]
        .lines()
        .map(|l| l.split_once("//").map_or(l, |(code, _)| code))
        .collect::<Vec<_>>()
        .join("\n");

    // `cmd.env(` does not match `cmd.env_clear(` or `cmd.env_remove(`.
    let clear_idx = body
        .find("cmd.env_clear()")
        .expect("build_command must call cmd.env_clear() for env isolation");
    let first_env_idx = body
        .find("cmd.env(")
        .expect("build_command must set env via cmd.env(...)");

    assert!(
        clear_idx < first_env_idx,
        "#1440 invariant violated: cmd.env_clear() (byte {clear_idx}) must precede \
         the first cmd.env(...) (byte {first_env_idx}) in build_command, or env \
         isolation would wipe explicitly-set keys"
    );
}
