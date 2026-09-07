//! #3541: `set_model` effort-dimension tests — split out of `set_model.rs`
//! so the handler file stays under the `file_size_invariant` MAX_LOC ceiling.
//! Helpers mirror the `set_model.rs` unit-test helpers (separate temp-dir tag
//! namespace, same semantics).

use super::set_model::handle_set_model;
use serde_json::json;
use std::path::Path;

fn test_home(tag: &str) -> std::path::PathBuf {
    let home =
        std::env::temp_dir().join(format!("agend-3541-setmodel-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).expect("create home");
    home
}

fn write_fleet(home: &Path, yaml: &str) {
    std::fs::write(crate::fleet::fleet_yaml_path(home), yaml).expect("write fleet.yaml");
}

fn fleet_text(home: &Path) -> String {
    std::fs::read_to_string(crate::fleet::fleet_yaml_path(home)).expect("read fleet.yaml")
}

const CODEX_SEAT: &str =
    "model_tiers:\n  cheap: claude-haiku-4-5\ninstances:\n  seat:\n    backend: codex\n    model_tier: cheap\n";

/// Collect every line of every .jsonl file under home (audit lives in the
/// daemon event log; the exact filename is an implementation detail).
fn walkdir_jsonl(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().is_some_and(|x| x == "jsonl") {
                if let Ok(text) = std::fs::read_to_string(&p) {
                    out.extend(text.lines().map(String::from));
                }
            }
        }
    }
    out
}

/// #3541: effort-only update — no model/tier touched, effort persists.
#[test]
fn set_model_effort_only_update_3541() {
    let home = test_home("effortonly");
    write_fleet(&home, CODEX_SEAT);
    let r = handle_set_model(&home, &json!({"instance": "seat", "effort": "high"}), &None);
    assert_eq!(r["persisted"], true, "got {r}");
    assert_eq!(r["set"]["effort"], "high", "got {r}");
    let text = fleet_text(&home);
    assert!(text.contains("effort: high"), "effort must persist: {text}");
    assert!(
        text.contains("model_tier: cheap"),
        "untouched model_tier must survive: {text}"
    );
    let _ = std::fs::remove_dir_all(&home);
}

/// #3541: combined model+effort update in ONE atomic transaction.
#[test]
fn set_model_combined_model_and_effort_3541() {
    let home = test_home("combined");
    write_fleet(&home, CODEX_SEAT);
    let r = handle_set_model(
        &home,
        &json!({"instance": "seat", "model": "o3", "effort": "low"}),
        &None,
    );
    assert_eq!(r["persisted"], true, "got {r}");
    assert_eq!(r["set"]["model"], "o3", "got {r}");
    assert_eq!(r["set"]["effort"], "low", "got {r}");
    let text = fleet_text(&home);
    assert!(text.contains("model: o3"), "model must persist: {text}");
    assert!(text.contains("effort: low"), "effort must persist: {text}");
    assert!(
        !text.contains("model_tier: cheap"),
        "model_tier must be cleared in the same write: {text}"
    );
    let _ = std::fs::remove_dir_all(&home);
}

/// #3541: empty-string effort clears the key; audit records was_effort.
#[test]
fn set_model_effort_clear_and_audit_3541() {
    let home = test_home("effortclear");
    write_fleet(
        &home,
        "instances:\n  seat:\n    backend: codex\n    model: o3\n    effort: high\n",
    );
    let r = handle_set_model(&home, &json!({"instance": "seat", "effort": ""}), &None);
    assert_eq!(r["persisted"], true, "got {r}");
    let text = fleet_text(&home);
    assert!(
        !text.contains("effort:"),
        "effort key must be removed: {text}"
    );
    assert!(text.contains("model: o3"), "model must survive: {text}");
    let hits: Vec<String> = walkdir_jsonl(&home)
        .into_iter()
        .filter(|l| l.contains("set_model") && l.contains("seat"))
        .collect();
    assert!(
        hits.iter().any(|l| l.contains("was_effort")),
        "audit must record was_effort, got {hits:?}"
    );
    let _ = std::fs::remove_dir_all(&home);
}

/// #3541: typo value outside the global domain is a hard error.
#[test]
fn set_model_invalid_effort_value_hard_errors_3541() {
    let home = test_home("badeffort");
    write_fleet(&home, CODEX_SEAT);
    let r = handle_set_model(
        &home,
        &json!({"instance": "seat", "effort": "ultra"}),
        &None,
    );
    assert_eq!(r["code"], "invalid_effort_value", "got {r}");
    assert!(
        !fleet_text(&home).contains("effort:"),
        "no effort key may be written on the error path"
    );
    let _ = std::fs::remove_dir_all(&home);
}

/// #3541: effort valid globally but narrower than this backend's range
/// persists (spawn fail-soft-drops it) — no silent no-op.
#[test]
fn set_model_effort_narrower_than_backend_persists_3541() {
    let home = test_home("narrower");
    write_fleet(&home, CODEX_SEAT);
    // Codex allows low|medium|high; `max` is global-valid (claude).
    let r = handle_set_model(&home, &json!({"instance": "seat", "effort": "max"}), &None);
    assert_eq!(r["persisted"], true, "got {r}");
    assert!(
        fleet_text(&home).contains("effort: max"),
        "portable intent must persist"
    );
    let _ = std::fs::remove_dir_all(&home);
}

/// #3541: effort on a backend with no effort capability persists with
/// an explicit warning flag (spawn drops it) instead of erroring.
#[test]
fn set_model_effort_unsupported_backend_warns_not_errors_3541() {
    let home = test_home("effortunsupported");
    write_fleet(&home, "instances:\n  k:\n    backend: kiro-cli\n");
    let r = handle_set_model(&home, &json!({"instance": "k", "effort": "low"}), &None);
    assert_eq!(r["persisted"], true, "got {r}");
    assert_eq!(r["effort_unsupported_dropped"], true, "got {r}");
    assert!(
        fleet_text(&home).contains("effort: low"),
        "intent must persist for a future backend switch"
    );
    let _ = std::fs::remove_dir_all(&home);
}

/// #3541: hand-written effort in args REJECTS (no silent no-op).
#[test]
fn set_model_rejects_effort_args_conflict_3541() {
    let home = test_home("effortconflict");
    write_fleet(
        &home,
        "instances:\n  seat:\n    backend: claude\n    args:\n      - --effort\n      - low\n",
    );
    let r = handle_set_model(&home, &json!({"instance": "seat", "effort": "high"}), &None);
    assert_eq!(r["code"], "args_conflict_confirmed", "got {r}");

    let home2 = test_home("effortconflictx");
    write_fleet(
            &home2,
            "instances:\n  cx:\n    backend: codex\n    args:\n      - -c\n      - model_reasoning_effort=\"low\"\n",
        );
    let r2 = handle_set_model(&home2, &json!({"instance": "cx", "effort": "high"}), &None);
    assert_eq!(r2["code"], "args_conflict_confirmed", "got {r2}");
    let _ = std::fs::remove_dir_all(&home);
    let _ = std::fs::remove_dir_all(&home2);
}

/// #3541: bare instance with no fields is a hard error (no silent
/// no-op); model+tier together still rejects as exactly-one.
#[test]
fn set_model_requires_at_least_one_field_3541() {
    let home = test_home("atleastone");
    write_fleet(&home, CODEX_SEAT);
    for bad in [
        json!({"instance": "seat"}),
        json!({"instance": "seat", "model": "o3", "tier": "cheap"}),
        json!({"instance": "seat", "restart": true}),
    ] {
        let r = handle_set_model(&home, &bad, &None);
        assert!(r["error"].as_str().is_some(), "want error, got {r}");
    }
    let _ = std::fs::remove_dir_all(&home);
}
