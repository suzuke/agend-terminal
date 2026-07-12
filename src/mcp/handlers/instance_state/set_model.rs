//! #2744 PR-A: `set_model` — typed, fleet-scoped explicit model intent.
//!
//! Contract (decision d-20260712101306674407-19): exactly one of
//! `model`/`tier` (handler-enforced); persists ONLY to the target instance's
//! fleet.yaml entry; atomically sets one field and clears the other in a
//! single lock/write transaction; every false/skip persistence outcome is a
//! hard tool error; default no restart (`restart:true` opts in, and a restart
//! failure after a durable persist reports `persisted:true, restart_ok:false`
//! — never a rollback); ACL = instance management (anonymous/operator full
//! authority; identified caller must be the instance itself, its team
//! orchestrator, or its creator); capability keys off the DECLARED backend
//! (Shell/Raw/custom hard-error); pre-existing model-flag spellings in the
//! entry's args are hard conflicts (parser-aware; no automatic argv
//! rewriting).

use serde_json::{json, Value};
use std::path::Path;

pub(crate) fn handle_set_model(
    home: &Path,
    args: &Value,
    sender: &Option<crate::identity::Sender>,
) -> Value {
    let _ = (home, args, sender);
    json!({"error": "set_model: unimplemented (#2744 PR-A C4)", "code": "unimplemented"})
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_home(tag: &str) -> std::path::PathBuf {
        let home =
            std::env::temp_dir().join(format!("agend-2744-setmodel-{tag}-{}", std::process::id()));
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

    fn sender(name: &str) -> Option<crate::identity::Sender> {
        Some(crate::identity::Sender::new(name).expect("sender"))
    }

    const CODEX_SEAT: &str =
        "model_tiers:\n  cheap: claude-haiku-4-5\ninstances:\n  seat:\n    backend: codex\n    model_tier: cheap\n";

    /// T2: exactly one of model/tier — both or neither is a hard error.
    #[test]
    fn set_model_enforces_exactly_one_of_model_or_tier_2744() {
        let home = test_home("exclusive");
        write_fleet(&home, CODEX_SEAT);
        for bad in [
            json!({"instance": "seat", "model": "o3", "tier": "cheap"}),
            json!({"instance": "seat"}),
        ] {
            let r = handle_set_model(&home, &bad, &None);
            assert!(
                r["error"]
                    .as_str()
                    .is_some_and(|e| e.contains("exactly one")),
                "want exactly-one error, got {r}"
            );
        }
        let _ = std::fs::remove_dir_all(&home);
    }

    /// T5: unknown instance is a hard error (no silent no-op — the
    /// `update_instance_field` Ok(false) class must surface).
    #[test]
    fn set_model_unknown_instance_hard_errors_2744() {
        let home = test_home("unknown");
        write_fleet(&home, CODEX_SEAT);
        let r = handle_set_model(&home, &json!({"instance": "ghost", "model": "o3"}), &None);
        assert!(
            r["error"].as_str().is_some_and(|e| e.contains("ghost")),
            "want unknown-instance error, got {r}"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    /// G4: declared Shell/Raw have no model capability — hard error, and the
    /// yaml is untouched.
    #[test]
    fn set_model_shell_raw_hard_error_2744() {
        let home = test_home("shellraw");
        write_fleet(
            &home,
            "instances:\n  sh-seat:\n    backend: shell\n  raw-seat:\n    backend: /opt/custom/bin\n",
        );
        let before = fleet_text(&home);
        for seat in ["sh-seat", "raw-seat"] {
            let r = handle_set_model(&home, &json!({"instance": seat, "model": "x"}), &None);
            assert!(
                r["error"]
                    .as_str()
                    .is_some_and(|e| e.contains("model capability")),
                "{seat}: want capability error, got {r}"
            );
        }
        assert_eq!(before, fleet_text(&home), "yaml must be untouched");
        let _ = std::fs::remove_dir_all(&home);
    }

    /// T2: a tier must exist (non-empty) in model_tiers at write time.
    #[test]
    fn set_model_unknown_tier_hard_errors_2744() {
        let home = test_home("badtier");
        write_fleet(&home, CODEX_SEAT);
        let r = handle_set_model(&home, &json!({"instance": "seat", "tier": "nope"}), &None);
        assert!(
            r["error"].as_str().is_some_and(|e| e.contains("nope")),
            "want unknown-tier error, got {r}"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    /// T4 confirmed conflict: an explicit model flag in the entry's args
    /// REJECTS (no rewrite, no success-while-args-wins).
    #[test]
    fn set_model_rejects_confirmed_args_conflict_2744() {
        let home = test_home("confirmed");
        write_fleet(
            &home,
            "instances:\n  seat:\n    backend: codex\n    args:\n      - -m\n      - pinned\n",
        );
        let r = handle_set_model(&home, &json!({"instance": "seat", "model": "o3"}), &None);
        assert_eq!(r["code"], "args_conflict_confirmed", "got {r}");
        assert!(
            r["error"].as_str().is_some_and(|e| e.contains("-m")),
            "error must name the conflicting token, got {r}"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    /// T4 ambiguous conflict: a glued `-mVAL` token on a `-m`-declaring
    /// backend rejects with the AMBIGUOUS class and a disambiguation hint
    /// (move payload after `--`).
    #[test]
    fn set_model_rejects_ambiguous_args_conflict_with_hint_2744() {
        let home = test_home("ambiguous");
        write_fleet(
            &home,
            "instances:\n  seat:\n    backend: codex\n    args:\n      - -mpinned\n",
        );
        let r = handle_set_model(&home, &json!({"instance": "seat", "model": "o3"}), &None);
        assert_eq!(r["code"], "args_conflict_ambiguous", "got {r}");
        assert!(
            r["error"].as_str().is_some_and(|e| e.contains("--")),
            "error must hint at the -- delimiter, got {r}"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    /// T4 false-positive pins: `-m` on a long-flag-only backend (claude) and
    /// model-flag lookalikes AFTER a bare `--` are NOT conflicts.
    #[test]
    fn set_model_no_false_positive_conflicts_2744() {
        let home = test_home("falsepos");
        write_fleet(
            &home,
            "instances:\n  cl:\n    backend: claude\n    args:\n      - -m\n      - notaflag\n  cx:\n    backend: codex\n    args:\n      - --\n      - --model\n      - payload\n",
        );
        for seat in ["cl", "cx"] {
            let r = handle_set_model(
                &home,
                &json!({"instance": seat, "model": "claude-opus-4-8"}),
                &None,
            );
            assert!(
                r["error"].is_null(),
                "{seat}: lookalike token must not conflict, got {r}"
            );
        }
        let _ = std::fs::remove_dir_all(&home);
    }

    /// Core semantics: set model clears model_tier (and vice versa) in the
    /// SAME write; the response reports what was set and cleared.
    #[test]
    fn set_model_atomically_clears_tier_and_vice_versa_2744() {
        let home = test_home("mutualclear");
        write_fleet(&home, CODEX_SEAT);

        let r = handle_set_model(&home, &json!({"instance": "seat", "model": "o3"}), &None);
        assert_eq!(r["persisted"], true, "got {r}");
        let text = fleet_text(&home);
        assert!(text.contains("model: o3"), "model must persist: {text}");
        assert!(
            !text.contains("model_tier"),
            "model_tier must be cleared in the same write: {text}"
        );

        let r = handle_set_model(&home, &json!({"instance": "seat", "tier": "cheap"}), &None);
        assert_eq!(r["persisted"], true, "got {r}");
        let text = fleet_text(&home);
        assert!(
            text.contains("model_tier: cheap"),
            "tier must persist: {text}"
        );
        assert!(
            !text.contains("model: o3"),
            "model must be cleared in the same write: {text}"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    /// ACL: identified unrelated caller is denied; the instance itself is
    /// allowed; anonymous (operator-direct) is allowed.
    #[test]
    fn set_model_acl_matches_instance_management_2744() {
        let home = test_home("acl");
        write_fleet(&home, CODEX_SEAT);
        let params = json!({"instance": "seat", "model": "o3"});

        let r = handle_set_model(&home, &params, &sender("bystander"));
        assert!(
            r["error"]
                .as_str()
                .is_some_and(|e| e.contains("permission denied")),
            "unrelated caller must be denied, got {r}"
        );

        let r = handle_set_model(&home, &params, &sender("seat"));
        assert_eq!(r["persisted"], true, "self must be allowed, got {r}");

        let r = handle_set_model(&home, &json!({"instance": "seat", "tier": "cheap"}), &None);
        assert_eq!(r["persisted"], true, "operator must be allowed, got {r}");
        let _ = std::fs::remove_dir_all(&home);
    }

    /// ACL: the team orchestrator of the target IS allowed.
    #[test]
    fn set_model_acl_allows_team_orchestrator_2744() {
        let home = test_home("aclorch");
        write_fleet(
            &home,
            "model_tiers:\n  cheap: claude-haiku-4-5\nteams:\n  crew:\n    orchestrator: boss\n    members:\n      - seat\ninstances:\n  boss:\n    backend: claude\n  seat:\n    backend: codex\n",
        );
        let r = handle_set_model(
            &home,
            &json!({"instance": "seat", "model": "o3"}),
            &sender("boss"),
        );
        assert_eq!(r["persisted"], true, "orchestrator allowed, got {r}");
        let _ = std::fs::remove_dir_all(&home);
    }

    /// restart=true fires only AFTER durable persistence; a restart failure
    /// (instance not running here) must NOT roll back or mask the persist:
    /// `persisted:true, restart_ok:false` + actionable error.
    #[test]
    fn set_model_restart_failure_reports_persisted_true_2744() {
        let home = test_home("restartfail");
        write_fleet(&home, CODEX_SEAT);
        let r = handle_set_model(
            &home,
            &json!({"instance": "seat", "model": "o3", "restart": true}),
            &None,
        );
        assert_eq!(r["persisted"], true, "persist must survive, got {r}");
        assert_eq!(
            r["restart_ok"], false,
            "restart must report failure, got {r}"
        );
        assert!(
            fleet_text(&home).contains("model: o3"),
            "no rollback on restart failure"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    /// Durable audit: a successful write appends a set_model event to the
    /// daemon event log (not just a tracing line).
    #[test]
    fn set_model_writes_durable_audit_event_2744() {
        let home = test_home("audit");
        write_fleet(&home, CODEX_SEAT);
        let r = handle_set_model(&home, &json!({"instance": "seat", "model": "o3"}), &None);
        assert_eq!(r["persisted"], true, "got {r}");
        let hits: Vec<String> = walkdir_jsonl(&home)
            .into_iter()
            .filter(|l| l.contains("set_model") && l.contains("seat"))
            .collect();
        assert!(
            !hits.is_empty(),
            "expected a durable set_model audit line under {home:?}"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

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
}
