//! #t-…83936-4 protection ① — `binding_state` canonical/gitdir liveness tests.
//!
//! Located in this sibling file (loaded via `#[path]` from binding_state.rs) per
//! the Sprint 54/55 `file_size_invariant` pattern (channel.rs / comms.rs use the
//! same idiom) so the handler's production file stays under the 750-LOC ceiling.
//!
//! Pins the additive `worktree_resolves` + `invalid_reason` fields that make the
//! 40-min-silent canonical-deletion incident detectable: a linked worktree's
//! `.git` is a POINTER file that OUTLIVES its canonical, so `worktree_valid`
//! (which only checks `.git` *exists*) stays falsely true. Per lead Q1 the new
//! fields are ADDITIVE — `worktree_valid` semantics are untouched.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::handle_binding_state;
use serde_json::json;
use std::path::Path;

fn tmp_home(suffix: &str) -> std::path::PathBuf {
    let h = std::env::temp_dir().join(format!(
        "agend-binding-live-{}-{}",
        std::process::id(),
        suffix
    ));
    std::fs::create_dir_all(&h).ok();
    h
}

/// Write a `binding.json` with a caller-chosen `source_repo` — the liveness
/// checks need to point `source_repo` at a real / a deleted canonical to
/// exercise `canonical_present`.
fn write_binding_src(home: &Path, agent: &str, branch: &str, worktree: &str, source_repo: &str) {
    let dir = crate::paths::runtime_dir(home).join(agent);
    std::fs::create_dir_all(&dir).unwrap();
    let payload = json!({
        "version": 1,
        "agent": agent,
        "task_id": "test-task",
        "branch": branch,
        "worktree": worktree,
        "source_repo": source_repo,
        "issued_at": "2026-05-09T00:00:00Z",
    });
    std::fs::write(
        dir.join("binding.json"),
        serde_json::to_string_pretty(&payload).unwrap(),
    )
    .unwrap();
}

/// Initialise a real git repo (test fixture). `AGEND_GIT_BYPASS=1` satisfies the
/// git-test-bypass invariant; this is a `#[cfg(test)]`-only file so the
/// daemon-git-helper scanner (production-portion only) never sees the raw git.
fn git_init(dir: &Path) {
    std::process::Command::new("git")
        .args(["init", "-q"])
        .current_dir(dir)
        .env("AGEND_GIT_BYPASS", "1")
        .output()
        .expect("git init");
}

/// RED (protection ①): the incident's soul at the binding_state layer. A linked
/// worktree's `.git` is a POINTER file that OUTLIVES the canonical it points to,
/// so `worktree_valid` — which only checks that `.git` *exists* — stays falsely
/// `true` after the canonical is gone (the 40-min-silent incident). The additive
/// `worktree_resolves` field (lead Q1: `worktree_valid` semantics UNTOUCHED)
/// resolves the pointer for a real liveness verdict, and `invalid_reason` names
/// the failure. Both fields are new, so every assertion here fails RED against
/// the pre-fix handler (the keys are absent → `as_bool()`/`as_str()` → `None`).
#[test]
fn binding_state_worktree_resolves_and_invalid_reason() {
    let home = tmp_home("resolves");
    // A real canonical git repo → `canonical_present` is true in the happy and
    // gitdir-dangling cases (isolates them from `canonical_missing`).
    let canonical = home.join("canonical");
    std::fs::create_dir_all(&canonical).unwrap();
    git_init(&canonical);
    let canonical_str = canonical.to_str().unwrap();

    // ── Happy: a worktree whose gitdir resolves + a live canonical.
    let wt_ok = home.join("wt-ok");
    std::fs::create_dir_all(&wt_ok).unwrap();
    git_init(&wt_ok);
    write_binding_src(
        &home,
        "hap",
        "feature/x",
        wt_ok.to_str().unwrap(),
        canonical_str,
    );
    let r = handle_binding_state(&home, &json!({"instance": "hap"}), &None);
    assert_eq!(r["worktree_valid"].as_bool(), Some(true), "{r}");
    assert_eq!(
        r["worktree_resolves"].as_bool(),
        Some(true),
        "resolving worktree + live canonical → alive: {r}"
    );
    assert!(r["invalid_reason"].is_null(), "healthy → no reason: {r}");

    // ── gitdir_dangling (SOUL): the `.git` pointer file exists but points at a
    // since-deleted admin dir; the canonical itself is still present.
    // `worktree_valid` stays TRUE (pointer file present — untouched semantics)
    // while `worktree_resolves` is FALSE. Exactly the dangling gitdir that went
    // silent for 40 minutes.
    let wt_dangle = home.join("wt-dangle");
    std::fs::create_dir_all(&wt_dangle).unwrap();
    std::fs::write(
        wt_dangle.join(".git"),
        format!("gitdir: {}/never-existed-admin\n", home.display()),
    )
    .unwrap();
    write_binding_src(
        &home,
        "dng",
        "feature/x",
        wt_dangle.to_str().unwrap(),
        canonical_str,
    );
    let r = handle_binding_state(&home, &json!({"instance": "dng"}), &None);
    assert_eq!(
        r["worktree_valid"].as_bool(),
        Some(true),
        "the `.git` pointer file exists → valid stays true (semantics untouched): {r}"
    );
    assert_eq!(
        r["worktree_resolves"].as_bool(),
        Some(false),
        "pointer resolves to a deleted admin dir → NOT alive: {r}"
    );
    assert_eq!(
        r["invalid_reason"].as_str(),
        Some("gitdir_dangling"),
        "canonical present but gitdir dangles: {r}"
    );

    // ── canonical_missing: the worktree resolves, but `source_repo` is gone.
    let wt_live = home.join("wt-live");
    std::fs::create_dir_all(&wt_live).unwrap();
    git_init(&wt_live);
    write_binding_src(
        &home,
        "cm",
        "feature/x",
        wt_live.to_str().unwrap(),
        home.join("no-such-canonical").to_str().unwrap(),
    );
    let r = handle_binding_state(&home, &json!({"instance": "cm"}), &None);
    assert_eq!(
        r["worktree_resolves"].as_bool(),
        Some(false),
        "canonical gone → not alive even though the worktree itself resolves: {r}"
    );
    assert_eq!(
        r["invalid_reason"].as_str(),
        Some("canonical_missing"),
        "{r}"
    );

    // ── worktree_missing: the worktree dir itself was never created.
    write_binding_src(
        &home,
        "wm",
        "feature/x",
        home.join("never").to_str().unwrap(),
        canonical_str,
    );
    let r = handle_binding_state(&home, &json!({"instance": "wm"}), &None);
    assert_eq!(
        r["worktree_resolves"].as_bool(),
        Some(false),
        "no worktree → not alive: {r}"
    );
    assert_eq!(
        r["invalid_reason"].as_str(),
        Some("worktree_missing"),
        "{r}"
    );

    std::fs::remove_dir_all(&home).ok();
}
